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

use crate::auth::{AuthContext, AuthError, Token, User};
use crate::journal::{JournalError, JournalRecord, ListCursor, UnmatchedCursor, UnmatchedRecord};
use crate::registry::{Group, NewGroup, NewRoute, RegistryError, Route, render_slug};
use crate::server::is_reserved_path;
use crate::{AppState, SUPPORTED_BINDINGS_VERSION};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/__api/routes", post(create_route).get(list_routes))
        .route(
            "/__api/routes/{group}/{number}",
            get(get_route).delete(delete_route),
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
struct CreateRouteBody {
    group: Option<String>,
    methods: Vec<String>,
    path: String,
    language: String,
    bindings_version: Option<String>,
    /// Base64-encoded `.component.wasm` bytes. Required when
    /// `language == "wasm"`.
    compiled_wasm: Option<String>,
    /// Source code for the source-based path. Forwarded to the compiler
    /// sidecar; returns `compile_failed` if no sidecar is configured.
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
        }
    }
}

#[derive(Debug, Serialize)]
struct ListRoutesResponse {
    routes: Vec<RouteResponse>,
}

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

// Bearer-token extractor. Pulls `Authorization: Bearer wmt_...` from the
// request, looks up the token via Auth, and returns 401 on missing /
// invalid / expired. Used by every handler under `/__api/*` that needs
// caller identity.
impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(header_value) = parts.headers.get(header::AUTHORIZATION) else {
            return Err(ApiError::unauthorized("missing Authorization header"));
        };
        let raw = header_value
            .to_str()
            .map_err(|_| ApiError::unauthorized("Authorization header is not valid ASCII"))?;
        let token = raw
            .strip_prefix("Bearer ")
            .ok_or_else(|| ApiError::unauthorized("expected `Bearer wmt_...` scheme"))?
            .trim();
        let ctx = state
            .auth()
            .authenticate(token)
            .map_err(|e| ApiError::internal(format!("auth lookup: {e}")))?
            .ok_or_else(|| ApiError::unauthorized("invalid or expired token"))?;
        Ok(ctx)
    }
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

async fn list_routes(
    State(state): State<AppState>,
    _auth: AuthContext,
) -> Result<Json<ListRoutesResponse>, ApiError> {
    let snapshot = state.routes().snapshot();
    let routes = snapshot.iter().map(RouteResponse::from).collect();
    Ok(Json(ListRoutesResponse { routes }))
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
}

#[derive(Debug, Deserialize, Default)]
struct UnmatchedListQuery {
    before: Option<u64>,
    limit: Option<usize>,
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
    let cursor = ListCursor {
        before: q.before,
        limit: q.limit.unwrap_or(100),
    };
    let entries = state.journal().list_for_group(&group_record.id, cursor)?;
    let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
    Ok(Json(ListJournalResponse {
        entries,
        next_before,
    }))
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
    let cursor = UnmatchedCursor {
        before: q.before,
        limit: q.limit.unwrap_or(100),
    };
    let entries = state.journal().list_unmatched(cursor)?;
    let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
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
        }
    }
}

#[derive(Debug, Serialize)]
struct ListGroupsResponse {
    groups: Vec<GroupResponse>,
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
) -> Result<Json<ListGroupsResponse>, ApiError> {
    let groups = if auth.is_admin {
        state.routes().registry().list_groups()?
    } else {
        state
            .routes()
            .registry()
            .list_groups_by_owner(&auth.user_id)?
    };
    let groups = groups.iter().map(GroupResponse::from).collect();
    Ok(Json(ListGroupsResponse { groups }))
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

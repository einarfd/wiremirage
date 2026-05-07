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
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use crate::auth::{AuthContext, AuthError, Token};
use crate::registry::{NewRoute, RegistryError, Route, render_slug};
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

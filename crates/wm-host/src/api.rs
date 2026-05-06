//! REST API at `/__api/routes`.
//!
//! POST/GET/DELETE for routes. Body has two shapes per `rest-api.md`:
//! pre-compiled (`language: "wasm"`, `compiled_wasm` base64) and
//! source-based (`language: "typescript"|...`, `source`). Source-based
//! requests forward to the compiler sidecar via `CompilerClient`; if no
//! sidecar is configured, those requests fail with `compile_failed`.
//!
//! No auth on the request handlers themselves yet — the host refuses to
//! start without `WM_INSECURE_NO_AUTH=1`, so a deployer can't enable this
//! by accident. Real auth + `WM_API_TOKEN` arrive in a follow-up slice.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

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
struct ApiError {
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

async fn list_routes(State(state): State<AppState>) -> Result<Json<ListRoutesResponse>, ApiError> {
    let snapshot = state.routes().snapshot();
    let routes = snapshot.iter().map(RouteResponse::from).collect();
    Ok(Json(ListRoutesResponse { routes }))
}

async fn get_route(
    State(state): State<AppState>,
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
    Path((group, number)): Path<(String, u32)>,
) -> Result<StatusCode, ApiError> {
    // Look up first so we can invalidate the route table after deletion.
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    state.routes().registry().delete_route(&group, number)?;
    state.routes().refresh_after_delete(&route.id);
    Ok(StatusCode::NO_CONTENT)
}

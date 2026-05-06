use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use http::header;

use crate::Runtime;
use crate::bindings::wiremirage::handler::http::{Header, Request as WitRequest};
use crate::route_table::RouteTable;

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
}

impl AppState {
    pub fn new(runtime: Arc<Runtime>, routes: Arc<RouteTable>) -> Self {
        Self { runtime, routes }
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub fn routes(&self) -> &Arc<RouteTable> {
        &self.routes
    }
}

/// Build the axum router. The REST API mounts at `/__api/*`; mock-traffic
/// dispatch is the fallback. The fallback rejects requests under reserved
/// prefixes (e.g., `/__api/typo`) with 404 before consulting user routes.
pub fn router(state: AppState) -> Router {
    crate::api::router()
        .fallback(any(dispatch))
        .with_state(state)
}

pub fn is_reserved_path(path: &str) -> bool {
    RESERVED_EXACT.contains(&path) || RESERVED_PREFIXES.iter().any(|&p| path.starts_with(p))
}

async fn dispatch(State(state): State<AppState>, req: Request) -> Response {
    match dispatch_inner(state, req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e, "handler invocation failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"))
        }
    }
}

async fn dispatch_inner(state: AppState, req: Request) -> anyhow::Result<Response> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let header_map = req.headers().clone();
    let body_bytes = read_body(req.into_body()).await?;

    let path = uri.path();

    // Reserved paths never reach user routes. Any sub-router (slice 3+
    // mounts /__api/* here) takes precedence; if nothing else matched, a
    // request under a reserved prefix is a typo, not mock traffic.
    if is_reserved_path(path) {
        return Ok(not_found_response("reserved path"));
    }

    let matched = match state.routes.find_match(method.as_str(), path) {
        Some(m) => m,
        None => return Ok(not_found_response("no route matched")),
    };

    let component = state.routes.component_for(&matched.route)?;
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

    let wit_response = tokio::task::spawn_blocking(move || {
        let (handler, mut store, handles) =
            runtime.instantiate(&component, &group_id, &route_id)?;
        handler.call_handle(&mut store, &wit_request, handles.route, handles.group)
    })
    .await??;

    Ok(build_axum_response(wit_response))
}

async fn read_body(body: Body) -> anyhow::Result<Vec<u8>> {
    use http_body_util::BodyExt;
    let collected = body.collect().await?;
    Ok(collected.to_bytes().to_vec())
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

fn build_axum_response(resp: crate::bindings::wiremirage::handler::http::Response) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = Bytes::from(resp.body);

    let mut builder = Response::builder().status(status);
    for (name, value) in resp.headers {
        if reserved_response_header(&name) {
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

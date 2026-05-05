use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use http::header;
use wasmtime::component::Component;

use crate::Runtime;
use crate::bindings::wiremirage::handler::http::{Header, Request as WitRequest};
use crate::store::MemBucket;

/// Slice 1 server state: a single hardcoded component is wired to a
/// catch-all route. Slice 2 turns this into a route-table lookup.
#[derive(Clone)]
pub struct AppState {
    runtime: Arc<Runtime>,
    component: Arc<Component>,
}

impl AppState {
    pub fn new(runtime: Arc<Runtime>, component: Component) -> Self {
        Self {
            runtime,
            component: Arc::new(component),
        }
    }
}

/// Build the axum router for slice 1 — a single catch-all that routes every
/// request to the configured component.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", any(dispatch))
        .route("/{*rest}", any(dispatch))
        .with_state(state)
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

    let wit_request = build_wit_request(&method, &uri, &header_map, body_bytes);

    let runtime = state.runtime.clone();
    let component = state.component.clone();

    let wit_response = tokio::task::spawn_blocking(move || {
        let (handler, mut store, handles) =
            runtime.instantiate_with(&component, MemBucket::new(), MemBucket::new())?;
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

fn build_wit_request(method: &Method, uri: &Uri, headers: &HeaderMap, body: Vec<u8>) -> WitRequest {
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
        // Slice 1 has a single catch-all, so matched-pattern is just "*".
        // Slice 2's route-table lookup will replace this with the real pattern.
        matched_pattern: "*".to_string(),
        path_params: vec![],
        // Query parsing arrives with the route table in slice 2.
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
            // Drop. Slice 2 will surface a warning in the journal.
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

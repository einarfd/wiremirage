//! Bearer-token middleware for the MCP route.
//!
//! Applies the same authentication policy as `/__api/*`: an
//! `Authorization: Bearer wmt_...` header must resolve to a live
//! token via [`Auth::authenticate`]. On success we inject the
//! resolved [`AuthContext`] into request extensions so tools can pull
//! it out via `Extension<AuthContext>`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::AuthContext;

pub async fn require_bearer(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(header_value) = req.headers().get(header::AUTHORIZATION) else {
        return (StatusCode::UNAUTHORIZED, "missing Authorization header").into_response();
    };
    let Ok(raw) = header_value.to_str() else {
        return (
            StatusCode::UNAUTHORIZED,
            "Authorization header is not valid ASCII",
        )
            .into_response();
    };
    let Some(token) = raw.strip_prefix("Bearer ") else {
        return (StatusCode::UNAUTHORIZED, "expected `Bearer wmt_...` scheme").into_response();
    };
    let token = token.trim();
    let ctx: Option<AuthContext> = match state.auth().authenticate(token) {
        Ok(ctx) => ctx,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth lookup: {e}"),
            )
                .into_response();
        }
    };
    let Some(ctx) = ctx else {
        return (StatusCode::UNAUTHORIZED, "invalid or expired token").into_response();
    };

    req.extensions_mut().insert(ctx);
    next.run(req).await
}

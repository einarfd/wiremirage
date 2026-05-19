//! Bearer-token middleware for the MCP route.
//!
//! Applies the same authentication policy as `/__api/*`: an
//! `Authorization: Bearer wmt_...` (or `wmm_...`) header must resolve
//! to a live token via [`Auth::authenticate`]. On success we inject
//! the resolved [`AuthContext`] into request extensions so tools can
//! pull it out via `Extension<AuthContext>`.
//!
//! Unauth'd requests get a `WWW-Authenticate: Bearer
//! resource_metadata="..."` header on the 401 (ADR-0019 slice D /
//! MCP spec 2025-06-18 §2.3) so native clients can run discovery
//! against the OAuth Authorization Server without prior knowledge.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::AuthContext;

pub async fn require_bearer(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers().clone();
    let Some(header_value) = headers.get(header::AUTHORIZATION) else {
        return unauthenticated(&state, &headers, "missing Authorization header");
    };
    let Ok(raw) = header_value.to_str() else {
        return unauthenticated(&state, &headers, "Authorization header is not valid ASCII");
    };
    let Some(token) = raw.strip_prefix("Bearer ") else {
        return unauthenticated(
            &state,
            &headers,
            "expected `Bearer wmt_...` or `Bearer wmm_...` scheme",
        );
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
        return unauthenticated(&state, &headers, "invalid or expired token");
    };

    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// Build a 401 response carrying the `WWW-Authenticate` discovery
/// hint. The URL is derived from the same Host + `X-Forwarded-Proto`
/// inputs the `.well-known/*` handlers use, so a client running
/// discovery from the header value lands on a URL that matches the
/// AS the host is actually advertising.
fn unauthenticated(state: &AppState, headers: &HeaderMap, message: &'static str) -> Response {
    let base = derive_public_base(headers, state.trust_forwarded_headers());
    let metadata = format!("{base}/.well-known/oauth-protected-resource");
    let www_authenticate = format!("Bearer realm=\"wm-host\", resource_metadata=\"{metadata}\"");

    let mut resp = (StatusCode::UNAUTHORIZED, message).into_response();
    if let Ok(value) = HeaderValue::from_str(&www_authenticate) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    resp
}

fn derive_public_base(headers: &HeaderMap, trust_forwarded: bool) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    let scheme = if trust_forwarded
        && let Some(v) = headers.get("x-forwarded-proto")
        && let Ok(s) = v.to_str()
        && !s.is_empty()
    {
        s.to_string()
    } else {
        "http".to_string()
    };
    format!("{scheme}://{host}")
}

//! Auth-redirect middleware for `/__ui/*`.
//!
//! When a browser hits a UI URL without a valid session cookie, the
//! standard `AuthContext` extractor would respond with `401
//! Unauthorized` — fine for `/__api/*` but a dead-end UX for a
//! browser. This middleware intercepts requests under `/__ui/*` and
//! redirects to `/__auth/login?next=<original_path>` instead, then
//! the `/__auth/login/password` handler honours `next` after a
//! successful login.
//!
//! Static-asset URLs (`/__ui/static/*`) are excluded from this
//! middleware in `ui::router()` — they're served unauthenticated.

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::session::COOKIE_NAME;

/// Extract `wm_session=...` from any `Cookie` header on the request.
fn pick_session_cookie(req: &Request) -> Option<String> {
    for header_value in req.headers().get_all(header::COOKIE).iter() {
        let Ok(raw) = header_value.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix(&format!("{COOKIE_NAME}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Encode `path` for embedding in the login redirect's `next=` query
/// parameter. Only `/__ui/*` paths reach this middleware so the input
/// alphabet is tame, but we percent-encode anything outside the
/// query-safe set defensively.
fn encode_next(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub async fn require_session(
    State(state): State<AppState>,
    req: Request,
    next_layer: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let cookie = pick_session_cookie(&req);

    let authed = match (state.sessions(), cookie) {
        (Some(sessions), Some(value)) => sessions.touch(&value).is_ok(),
        _ => false,
    };

    if authed {
        next_layer.run(req).await
    } else {
        let next = encode_next(&path);
        let url = format!("/__auth/login?next={next}");
        Redirect::to(&url).into_response()
    }
}

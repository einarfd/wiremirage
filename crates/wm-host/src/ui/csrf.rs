//! Double-submit CSRF protection for authed UI forms.
//!
//! Pattern: every request reaching this middleware lands with either an
//! existing `wm_csrf` cookie or none. The middleware:
//!
//!   * On a "safe" method (`GET`/`HEAD`/`OPTIONS`): mints a token if no
//!     cookie was present, stashes the current value in a request
//!     extension so handlers can embed it in rendered forms, and adds
//!     `Set-Cookie` to the response if the cookie needed minting.
//!   * On a "mutating" method (`POST`/`PUT`/`PATCH`/`DELETE`): reads the
//!     request body, parses it as `application/x-www-form-urlencoded`,
//!     extracts the `_csrf` field, and rejects with 403 if it doesn't
//!     match the cookie. Then rebuilds the request with the same body
//!     for the downstream handler.
//!
//! The cookie is `SameSite=Strict; HttpOnly` — cross-site requests
//! don't carry it at all, and a script in the page can't read it. The
//! form embeds the same value server-side, so the comparison succeeds
//! on legitimate same-origin submits and fails on every other shape of
//! request that could plausibly originate this code path.
//!
//! Wired up in `ui::router` and `auth_api::router`.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use rand::RngCore;

use crate::AppState;

pub const CSRF_COOKIE_NAME: &str = "wm_csrf";

// Task-local set by `csrf_middleware` for the duration of each request
// it wraps. `ui::render` reads it and flattens a `csrf_token` field
// into every rendered context so the logout button (in base.html) and
// every form template can embed the value without each handler having
// to plumb the extension through. Empty string outside of a middleware
// scope — `template renders without a real form anyway`.
tokio::task_local! {
    pub static CURRENT_CSRF: String;
}

/// Max body size for the CSRF body-parse step. Authed UI forms ship a
/// tiny amount of data (`_csrf` + a few name/value pairs); anything
/// larger is either an upload-shaped request (file uploads aren't
/// CSRF-relevant — they go via /__api/) or a malformed body. 64 KiB
/// is comfortably above any legitimate authed form.
const MAX_FORM_BYTES: usize = 64 * 1024;

/// Request extension type so handlers can read the current CSRF
/// token without re-parsing cookies. Always present after the
/// middleware runs.
#[derive(Clone, Debug)]
pub struct CsrfToken(pub String);

/// Mint a fresh CSRF token. 128 bits of randomness, URL-safe base64.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    B64URL.encode(bytes)
}

/// Pull `wm_csrf=<value>` out of any `Cookie` header on the request.
fn pick_csrf_cookie(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = value.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix(&format!("{CSRF_COOKIE_NAME}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Extract the `_csrf` field from a urlencoded form body. Returns
/// `None` if the field is absent or the body isn't parseable.
fn pick_csrf_from_form(body: &[u8]) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    for pair in body_str.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?;
        if key == "_csrf" {
            let raw = parts.next().unwrap_or("");
            // Form bodies percent-encode; URL-safe base64 happens to
            // share the alphabet so no decoding is needed in practice,
            // but be defensive in case a future minter widens the set.
            return Some(urlencoding::decode(raw).ok()?.into_owned());
        }
    }
    None
}

fn build_set_cookie(value: &str, secure: bool) -> String {
    // 24 h matches the session cookie's max age (slice 20). HttpOnly
    // is fine for double-submit — the form value is server-injected
    // so no JS needs to read the cookie. SameSite=Strict is the real
    // defense; with it, the cookie isn't carried on any cross-site
    // request, so forged POSTs fail the cookie-presence check. The
    // `Secure` flag is gated on `WM_TRUSTED_PROXY` (ADR-0027) so
    // dev workflows over plain HTTP don't lose the cookie.
    let suffix = if secure { "; Secure" } else { "" };
    format!("{CSRF_COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400{suffix}")
}

pub async fn csrf_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let (parts, body) = req.into_parts();

    let existing = pick_csrf_cookie(&parts.headers);
    let needs_set_cookie = existing.is_none();
    let active_token = existing.clone().unwrap_or_else(mint_token);
    let scope_value = active_token.clone();

    // Validate on mutating methods. Read + rebuild the body so the
    // downstream handler can still extract its own Form<>.
    let is_mutating = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if is_mutating {
        let cookie_value = match existing.as_deref() {
            Some(v) => v.to_string(),
            None => {
                return (
                    StatusCode::FORBIDDEN,
                    "csrf: missing wm_csrf cookie (start with a GET to mint one)",
                )
                    .into_response();
            }
        };
        let bytes = match axum::body::to_bytes(body, MAX_FORM_BYTES).await {
            Ok(b) => b,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "csrf: form body too large").into_response();
            }
        };
        let form_token = pick_csrf_from_form(&bytes);
        let matches = form_token.as_deref().is_some_and(|t| t == cookie_value);
        if !matches {
            return (StatusCode::FORBIDDEN, "csrf: token mismatch").into_response();
        }
        let mut req = Request::from_parts(parts, Body::from(bytes));
        req.extensions_mut().insert(CsrfToken(active_token));
        return CURRENT_CSRF
            .scope(scope_value, async move { next.run(req).await })
            .await;
    }

    // Safe methods: pass through, set cookie on the response if we
    // had to mint one.
    let mut req = Request::from_parts(parts, body);
    req.extensions_mut().insert(CsrfToken(active_token.clone()));
    let token_for_set_cookie = active_token.clone();
    let mut response = CURRENT_CSRF
        .scope(scope_value, async move { next.run(req).await })
        .await;
    if needs_set_cookie
        && let Ok(header_val) = HeaderValue::from_str(&build_set_cookie(
            &token_for_set_cookie,
            state.secure_cookies(),
        ))
    {
        response
            .headers_mut()
            .append(header::SET_COOKIE, header_val);
    }
    response
}

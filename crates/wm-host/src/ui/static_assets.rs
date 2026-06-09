//! Static CSS/JS asset serving under `/ui/static/*`.
//!
//! Assets are embedded at compile time via `include_bytes!` so the
//! binary stays self-contained. Cache headers are conservative:
//! `no-store` until we have content-hashed filenames, then we can
//! flip to `immutable, max-age=...`.
//!
//! Ace Editor (slice 41) is vendored under `static/ace/`; we serve
//! a fixed set of files (core + JS/TS modes + light/dark themes) so
//! the wildcard route can't be coaxed into serving arbitrary files
//! out of the binary's data segment.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

const WM_CSS: &[u8] = include_bytes!("static/wm.css");
const WM_ACE_JS: &[u8] = include_bytes!("static/wm-ace.js");

const ACE_CORE: &[u8] = include_bytes!("static/ace/ace.js");
const ACE_MODE_JS: &[u8] = include_bytes!("static/ace/mode-javascript.js");
const ACE_MODE_TS: &[u8] = include_bytes!("static/ace/mode-typescript.js");
const ACE_THEME_LIGHT: &[u8] = include_bytes!("static/ace/theme-github_light_default.js");
const ACE_THEME_DARK: &[u8] = include_bytes!("static/ace/theme-github_dark.js");

const JS_MIME: &str = "application/javascript; charset=utf-8";

pub async fn serve(Path(name): Path<String>) -> Response {
    let (bytes, mime) = match name.as_str() {
        "wm.css" => (WM_CSS, "text/css; charset=utf-8"),
        "wm-ace.js" => (WM_ACE_JS, JS_MIME),
        "ace/ace.js" => (ACE_CORE, JS_MIME),
        "ace/mode-javascript.js" => (ACE_MODE_JS, JS_MIME),
        "ace/mode-typescript.js" => (ACE_MODE_TS, JS_MIME),
        "ace/theme-github_light_default.js" => (ACE_THEME_LIGHT, JS_MIME),
        "ace/theme-github_dark.js" => (ACE_THEME_DARK, JS_MIME),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

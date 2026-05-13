//! Static CSS/JS asset serving under `/__ui/static/*`.
//!
//! Assets are embedded at compile time via `include_bytes!` so the
//! binary stays self-contained. The list is small on purpose (slice
//! 21: one CSS file, no JS yet — HTMX will land alongside the slice
//! that needs it). Cache headers are conservative: `no-store` until
//! we have content-hashed filenames, then we can flip to
//! `immutable, max-age=...`.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

const WM_CSS: &[u8] = include_bytes!("static/wm.css");

pub async fn serve(Path(name): Path<String>) -> Response {
    let (bytes, mime) = match name.as_str() {
        "wm.css" => (WM_CSS, "text/css; charset=utf-8"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

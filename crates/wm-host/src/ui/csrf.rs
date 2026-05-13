//! Double-submit CSRF protection for authed UI forms.
//!
//! **Slice 21 status: the wiring lives here but no authed UI form
//! exists yet — the login form is unauthenticated (an attacker
//! without credentials has nothing to forge), and every other
//! `/__ui/*` route in this slice is read-only.** The middleware will
//! be hooked up by the slice that adds the first authed UI form
//! (Tokens / Route creation in slice 25–26).
//!
//! Pattern (when active): every authed GET sets a `wm_csrf` cookie
//! with a random per-session value; every form embeds the same value
//! as a hidden `_csrf` input; on POST/PATCH/DELETE the middleware
//! validates `_csrf == cookie`. Sessions don't bind the token — same
//! value across forms is fine.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use rand::RngCore;

pub const CSRF_COOKIE_NAME: &str = "wm_csrf";

/// Mint a fresh CSRF token. Called by the first authed handler that
/// renders a form-bearing page; the value is then stored in the
/// `wm_csrf` cookie and embedded in the form as a hidden input.
#[allow(dead_code)] // wired up by the slice that adds the first authed form
pub fn mint_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    B64URL.encode(bytes)
}

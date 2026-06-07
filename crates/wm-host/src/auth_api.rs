//! `/__auth/*` HTTP endpoints. Unauthenticated by design: the login
//! page renders for anyone, and the password POST is the only way
//! to mint a session for local-auth users.
//!
//! Surface (slice 20):
//!
//!   GET  /__auth/login            login form (HTML); shows the
//!                                 password form when `WM_LOCAL_AUTH`
//!                                 is configured. OAuth provider
//!                                 buttons land alongside in a later
//!                                 slice.
//!   POST /__auth/login/password   credential check + session mint
//!   POST /__auth/logout           session revoke + cookie clear

use std::net::{IpAddr, Ipv4Addr};

use axum::Form;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use minijinja::context;
use rand::RngCore;
use serde::Deserialize;

use crate::AppState;
use crate::local_auth::VerifyError;
use crate::session::COOKIE_NAME;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/__auth/login", get(login_page))
        .route("/__auth/login/password", post(password_login))
        .route("/__auth/logout", post(logout))
        // GitHub OAuth (slice 50). Both are GETs — the browser does
        // navigation, GitHub posts back via 302 with the code in the
        // query string. No CSRF token needed because the `state`
        // parameter on the OAuth callback IS our CSRF defence (we
        // generate and validate it ourselves).
        .route("/__auth/start/github", get(start_github))
        .route("/__auth/callback", get(github_callback))
        // Every form-bearing endpoint in this router is `/__auth/*`,
        // so we can blanket-CSRF the lot. The login GET mints the
        // cookie; the POSTs (login + logout) validate it. The
        // middleware reads `state.secure_cookies()` to decide
        // whether to append `Secure` on the cookie it mints. The
        // OAuth start/callback routes are also under `/__auth/*` —
        // they don't carry form bodies but the middleware mints the
        // CSRF cookie on safe methods, which is fine to share with
        // the password login form.
        .layer(axum::middleware::from_fn_with_state(
            state,
            crate::ui::csrf::csrf_middleware,
        ))
}

#[derive(Debug, Deserialize)]
struct LoginPageQuery {
    /// Post-login redirect target carried through from the original
    /// `/__ui/*` navigation. Validated as host-relative when the
    /// form posts (see `password_login`).
    next: Option<String>,
}

async fn login_page(State(state): State<AppState>, Query(q): Query<LoginPageQuery>) -> Response {
    // The form only renders when local auth is wired up; with the
    // surface bare today, showing it unconditionally would be a UX
    // dead-end (`POST /__auth/login/password` would 503). The CSRF
    // middleware wraps this handler so `csrf_token` is in scope and
    // gets injected into the template context by `ui::render`.
    let local_enabled = !state.local_auth().is_empty();
    let github_enabled = state.github_oauth().is_some();
    let next = q
        .next
        .filter(|n| n.starts_with('/') && !n.starts_with("//"));
    crate::ui::render(
        &state,
        "login.html",
        context! {
            local_enabled => local_enabled,
            github_enabled => github_enabled,
            next => next,
            error => Option::<String>::None,
        },
    )
}

#[derive(Debug, Deserialize)]
struct PasswordLoginForm {
    username: String,
    password: String,
    /// Optional post-login redirect target. The handler validates
    /// that the value is a host-relative path (`/...`) so an open-
    /// redirect can't be smuggled through a crafted login link.
    next: Option<String>,
    /// Validated by the CSRF middleware before this handler runs;
    /// declared here only so `axum::Form` accepts the field.
    #[serde(rename = "_csrf")]
    _csrf: Option<String>,
}

async fn password_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PasswordLoginForm>,
) -> Response {
    // Resolve the caller's IP. Only honor `X-Forwarded-For` when the
    // proxy is trusted (`WM_TRUSTED_PROXY`, ADR-0027). The default (off) ignores
    // the header and uses a loopback placeholder, collapsing every
    // caller into one throttle bucket — fine for trusted-network
    // deployments where the operator owns every consumer, and
    // mandatory for any deployment where the host is directly
    // reachable since anyone can spoof the header.
    let ip = client_ip(&headers, state.trust_forwarded_headers());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let local = state.local_auth();
    let sessions = match state.sessions() {
        Some(s) => s,
        None => {
            // SESSION_SECRET not configured — local auth can't mint a
            // cookie. 503 makes the operator misconfiguration loud.
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "session store not configured; set SESSION_SECRET",
            )
                .into_response();
        }
    };

    if local.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "local auth not configured; set WM_LOCAL_AUTH",
        )
            .into_response();
    }

    // Throttle check first — a locked-out caller doesn't get to learn
    // anything about whether the username exists.
    if state.login_throttle().is_locked_out(ip) {
        return login_failure(StatusCode::TOO_MANY_REQUESTS, "too many failed attempts");
    }

    let role = match local.verify(&form.username, &form.password) {
        Ok(role) => role,
        Err(VerifyError::Invalid) => {
            if state.login_throttle().record_failure(ip) {
                tracing::warn!(%ip, "login throttle: ip locked out");
            }
            return login_failure(StatusCode::UNAUTHORIZED, "login failed");
        }
        Err(VerifyError::HashEngine(e)) => {
            tracing::error!(error = %e, "argon2 verification engine error");
            return login_failure(StatusCode::INTERNAL_SERVER_ERROR, "login failed");
        }
    };
    state.login_throttle().record_success(ip);

    // Upsert the user record + identity index. On every successful
    // login we sync `is_admin` from the env-var role (per ADR-0018:
    // "Admin role lives in the env var"). The user record itself
    // survives across env-var edits.
    let user = match state
        .auth()
        .upsert_local_user(&form.username, role.is_admin())
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "upsert local user");
            return login_failure(StatusCode::INTERNAL_SERVER_ERROR, "login failed");
        }
    };

    let ip_str = ip.to_string();
    let (_session, cookie_value) = match sessions.create(&user.id, "local", &ip_str, &user_agent) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "session create");
            return login_failure(StatusCode::INTERNAL_SERVER_ERROR, "login failed");
        }
    };

    let next = form
        .next
        .as_deref()
        .filter(|n| n.starts_with('/') && !n.starts_with("//"))
        .unwrap_or("/__ui/");

    let mut redirect: Response = Redirect::to(next).into_response();
    redirect.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format_set_cookie(
            &cookie_value,
            sessions.ttl_seconds(),
            state.secure_cookies(),
        ))
        .expect("ascii cookie header"),
    );
    redirect
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Best-effort delete on the session record, then issue a Set-Cookie
    // that clears the cookie regardless. A logout call without a
    // cookie is still success — idempotency matters here.
    if let Some(sessions) = state.sessions()
        && let Some(cookie) = parse_session_cookie(&headers)
    {
        let _ = sessions.delete_by_cookie(&cookie);
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    // The clear-cookie response carries the same attributes as the
    // mint-cookie response so browsers don't keep a `Secure` cookie
    // alive thinking it's a different cookie. `Max-Age=0` is what
    // triggers the delete.
    let clear = format_clear_cookie(state.secure_cookies());
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear).expect("ascii cookie header"),
    );
    resp
}

fn login_failure(status: StatusCode, message: &'static str) -> Response {
    // Vague body by design. The Set-Cookie header is left alone — a
    // failed login doesn't clear an existing valid session.
    (status, message).into_response()
}

fn parse_session_cookie(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = value.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix(&format!("{COOKIE_NAME}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Resolve the client's IP. When `trust_forwarded` is true, honor the
/// first hop in `X-Forwarded-For`; when false, ignore the header
/// (which can be set by any caller) and return the loopback
/// placeholder so the throttle still has *something* to key on.
fn client_ip(headers: &HeaderMap, trust_forwarded: bool) -> IpAddr {
    if trust_forwarded
        && let Some(v) = headers.get("x-forwarded-for")
        && let Ok(raw) = v.to_str()
    {
        // `X-Forwarded-For: client, proxy1, proxy2` — the first hop
        // is the client.
        if let Some(first) = raw.split(',').next()
            && let Ok(ip) = first.trim().parse::<IpAddr>()
        {
            return ip;
        }
    }
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
}

fn format_set_cookie(value: &str, max_age: u64, secure: bool) -> String {
    // `Secure` is conditional: a plain-HTTP dev deployment would
    // never get the cookie back if we set it, so we lean on an
    // operator switch (`WM_TRUSTED_PROXY`) rather than emitting it
    // unconditionally. Deployments behind a TLS edge MUST set it
    // — see the production-hardening section in README.
    let suffix = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{suffix}")
}

fn format_clear_cookie(secure: bool) -> String {
    let suffix = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{suffix}")
}

// -- GitHub OAuth ------------------------------------------------------------
//
// Two-step flow:
//   GET /__auth/start/github   → 302 to github.com/login/oauth/authorize
//   GET /__auth/callback       → exchange code, mint session, 302 to next
//
// CSRF is handled inside the OAuth protocol itself: we generate a
// random `state` nonce at start, stash it (with the post-login `next`
// path) in Valkey at `oauth_state:{nonce}` with a 10-minute TTL, and
// validate the round-trip on callback. A request to /__auth/callback
// without a matching state is rejected.
//
// The state record is one-shot: the callback handler deletes it
// before exchanging the code so a replay can't reuse it.

const OAUTH_STATE_TTL_SECONDS: u64 = 600;

#[derive(Debug, Deserialize)]
struct StartGithubQuery {
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// GitHub puts the error in the query string when the user
    /// rejects the authorization. We render it nicely.
    error: Option<String>,
    error_description: Option<String>,
}

async fn start_github(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<StartGithubQuery>,
) -> Response {
    let Some(github) = state.github_oauth() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub login not configured; set WM_GITHUB_CLIENT_ID and WM_GITHUB_CLIENT_SECRET",
        )
            .into_response();
    };

    let next = q
        .next
        .as_deref()
        .filter(|n| n.starts_with('/') && !n.starts_with("//"))
        .unwrap_or("/__ui/")
        .to_string();

    // 32 random bytes encoded urlsafe-base64 ≈ 43 chars; collision-safe.
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    let mut bucket = match state.auth().storage().admin_bucket() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "open admin bucket for oauth state");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let key = format!("oauth_state:{nonce}");
    if let Err(e) = bucket.set(&key, next.into_bytes()) {
        tracing::error!(error = %e, "persist oauth state");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
    }
    // Best-effort TTL — if Valkey errors here the stale state would
    // hang around but it can't actually be used to log in (the
    // session-mint step needs a valid GitHub code anyway).
    let _ = bucket.set_ttl(&key, OAUTH_STATE_TTL_SECONDS);

    let redirect_uri = derive_redirect_uri(&headers, state.trust_forwarded_headers());
    let authorize = github.authorize_url(&redirect_uri, &nonce);
    Redirect::to(&authorize).into_response()
}

async fn github_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GithubCallbackQuery>,
) -> Response {
    let Some(github) = state.github_oauth() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub login not configured",
        )
            .into_response();
    };

    if let Some(err) = q.error {
        // User denied authorization at GitHub, or GitHub rejected the
        // app. Bounce back to /__auth/login with a human-readable
        // error. The description is GitHub-supplied; we surface it
        // verbatim because operators benefit from knowing whether it
        // was "access_denied" vs "redirect_uri_mismatch" vs other.
        let desc = q.error_description.unwrap_or_default();
        tracing::warn!(error = %err, description = %desc, "GitHub callback returned error");
        return (
            StatusCode::UNAUTHORIZED,
            format!("GitHub login failed: {err} ({desc})"),
        )
            .into_response();
    }

    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing `code` parameter").into_response();
    };
    let Some(state_nonce) = q.state else {
        return (StatusCode::BAD_REQUEST, "missing `state` parameter").into_response();
    };

    // Validate state + consume one-shot.
    let next = match consume_oauth_state(&state, &state_nonce) {
        Ok(next) => next,
        Err(StateError::NotFound) => {
            return (
                StatusCode::BAD_REQUEST,
                "invalid or expired `state` parameter; restart the login flow",
            )
                .into_response();
        }
        Err(StateError::Storage(e)) => {
            tracing::error!(error = %e, "load oauth state");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };

    // SESSION_SECRET is required to mint cookies. The startup checker
    // already refuses to start without it when GitHub is configured,
    // but defend in depth.
    let Some(sessions) = state.sessions() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "session store not configured; set SESSION_SECRET",
        )
            .into_response();
    };

    let redirect_uri = derive_redirect_uri(&headers, state.trust_forwarded_headers());
    let access_token = match github.exchange_code(&code, &redirect_uri).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "github code exchange failed");
            return (
                StatusCode::UNAUTHORIZED,
                "GitHub login failed (code exchange)",
            )
                .into_response();
        }
    };
    let identity = match github.fetch_identity(&access_token).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "github identity fetch failed");
            return (
                StatusCode::UNAUTHORIZED,
                "GitHub login failed (identity fetch)",
            )
                .into_response();
        }
    };
    if let Err(deny) = github.check_allow(&identity) {
        tracing::warn!(login = %identity.login, "github login denied by allow-rules");
        return (StatusCode::FORBIDDEN, format!("{deny}")).into_response();
    }

    let is_admin = github.is_admin(&identity.login);
    let user = match state.auth().upsert_oauth_user(
        "github",
        &identity.id.to_string(),
        &identity.login,
        is_admin,
    ) {
        Ok(u) => u,
        Err(crate::auth::AuthError::NameTaken(n)) => {
            return (
                StatusCode::CONFLICT,
                format!(
                    "GitHub login {n:?} collides with an existing local user. \
                     Rename or delete the local user, then re-try login."
                ),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "upsert oauth user");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error (user upsert)",
            )
                .into_response();
        }
    };

    let ip_str = client_ip(&headers, state.trust_forwarded_headers()).to_string();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (_session, cookie_value) = match sessions.create(&user.id, "github", &ip_str, &user_agent) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "session create");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error (session mint)",
            )
                .into_response();
        }
    };

    let mut redirect: Response = Redirect::to(&next).into_response();
    redirect.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format_set_cookie(
            &cookie_value,
            sessions.ttl_seconds(),
            state.secure_cookies(),
        ))
        .expect("ascii cookie header"),
    );
    redirect
}

enum StateError {
    NotFound,
    Storage(crate::store::StoreError),
}

fn consume_oauth_state(state: &AppState, nonce: &str) -> Result<String, StateError> {
    let mut bucket = state
        .auth()
        .storage()
        .admin_bucket()
        .map_err(StateError::Storage)?;
    let key = format!("oauth_state:{nonce}");
    let Some(bytes) = bucket.get(&key).map_err(StateError::Storage)? else {
        return Err(StateError::NotFound);
    };
    let next = String::from_utf8(bytes).unwrap_or_else(|_| "/__ui/".to_string());
    // One-shot — delete before we exchange the code so a parallel
    // attempt with the same state can't ride along.
    let _ = bucket.delete(&key);
    // Re-validate the `next` path the same way `start_github` did.
    let safe_next = if next.starts_with('/') && !next.starts_with("//") {
        next
    } else {
        "/__ui/".to_string()
    };
    Ok(safe_next)
}

/// Build the redirect_uri we tell GitHub to send the user back to.
/// MUST match what's registered on the GitHub OAuth app's settings
/// page (within the loopback-port wildcard described in RFC 8252,
/// which GitHub honors for `http://localhost:<port>` URLs).
/// The host's public base URL (`{scheme}://{host}`), derived from the
/// request. `Host` is taken verbatim (a reverse proxy like Caddy
/// preserves it); the scheme comes from `X-Forwarded-Proto` when the
/// proxy is trusted (`WM_TRUSTED_PROXY`), else `http` for the loopback
/// dev case. Shared by the OAuth redirect-URI and the "Connect an agent"
/// page so both report the same public origin.
pub(crate) fn public_base_url(headers: &HeaderMap, trust_forwarded: bool) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    let scheme = if trust_forwarded
        && let Some(v) = headers.get("x-forwarded-proto")
        && let Ok(s) = v.to_str()
        && !s.is_empty()
    {
        s
    } else {
        "http"
    };
    format!("{scheme}://{host}")
}

/// The public base URL for a group's mock traffic: `{scheme}://{group}.{apex}`
/// (ADR-0030 virtual-host routing). Mock traffic is served on per-group
/// subdomains; the apex — where this control-plane request lands — serves
/// control-plane only. These callers are apex-only surfaces, so the request
/// `Host` *is* the apex; we reuse [`public_base_url`]'s scheme/host derivation
/// (which also carries the dev port, e.g. `localhost:8080`) and prefix the
/// group's DNS label. No trailing slash; append the route path to taste.
pub(crate) fn group_base_url(group: &str, headers: &HeaderMap, trust_forwarded: bool) -> String {
    let base = public_base_url(headers, trust_forwarded);
    match base.split_once("://") {
        Some((scheme, host)) => format!("{scheme}://{group}.{host}"),
        None => format!("{group}.{base}"),
    }
}

fn derive_redirect_uri(headers: &HeaderMap, trust_forwarded: bool) -> String {
    format!(
        "{}/__auth/callback",
        public_base_url(headers, trust_forwarded)
    )
}

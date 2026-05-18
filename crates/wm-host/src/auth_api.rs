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
use serde::Deserialize;

use crate::AppState;
use crate::local_auth::VerifyError;
use crate::session::COOKIE_NAME;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/__auth/login", get(login_page))
        .route("/__auth/login/password", post(password_login))
        .route("/__auth/logout", post(logout))
        // Every form-bearing endpoint in this router is `/__auth/*`,
        // so we can blanket-CSRF the lot. The login GET mints the
        // cookie; the POSTs (login + logout) validate it. The
        // middleware reads `state.secure_cookies()` to decide
        // whether to append `Secure` on the cookie it mints.
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
    let next = q
        .next
        .filter(|n| n.starts_with('/') && !n.starts_with("//"));
    crate::ui::render(
        &state,
        "login.html",
        context! {
            local_enabled => local_enabled,
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
    // Resolve the caller's IP. Slice 44: only honor `X-Forwarded-For`
    // when `WM_TRUST_FORWARDED_HEADERS=1`. The default (off) ignores
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
    // operator flag (`WM_SECURE_COOKIES`) rather than emitting it
    // unconditionally. Deployments behind a TLS edge MUST set the
    // flag — see the production-hardening section in README.
    let suffix = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{suffix}")
}

fn format_clear_cookie(secure: bool) -> String {
    let suffix = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{suffix}")
}

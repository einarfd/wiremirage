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
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::AppState;
use crate::local_auth::VerifyError;
use crate::session::COOKIE_NAME;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/__auth/login", get(login_page))
        .route("/__auth/login/password", post(password_login))
        .route("/__auth/logout", post(logout))
}

async fn login_page(State(state): State<AppState>) -> Response {
    // The form only renders when local auth is wired up; with the
    // surface bare today, showing it unconditionally would be a UX
    // dead-end (`POST /__auth/login/password` would 503).
    let body = if state.local_auth().is_empty() {
        DISABLED_PAGE.to_string()
    } else {
        ENABLED_PAGE.to_string()
    };
    Html(body).into_response()
}

#[derive(Debug, Deserialize)]
struct PasswordLoginForm {
    username: String,
    password: String,
    /// Optional post-login redirect target. The handler validates
    /// that the value is a host-relative path (`/...`) so an open-
    /// redirect can't be smuggled through a crafted login link.
    next: Option<String>,
}

async fn password_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PasswordLoginForm>,
) -> Response {
    // Resolve the caller's IP. We don't wire `ConnectInfo` into the
    // server (would require updating every test harness), so we lean
    // on `X-Forwarded-For` when a reverse proxy is in front and fall
    // back to a loopback placeholder otherwise. The throttle still
    // works in both cases — direct local-network deployments share
    // one bucket per actually-distinct external IP.
    let ip = client_ip(&headers);
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
        HeaderValue::from_str(&format_set_cookie(&cookie_value, sessions.ttl_seconds()))
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
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("wm_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
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

/// Resolve the client's IP. Trusts `X-Forwarded-For` when a reverse
/// proxy is in front; falls back to a loopback placeholder otherwise.
/// The throttle keys to this value — the placeholder collapses many
/// direct-connection callers into a single bucket, which is fine for
/// the threat model (operator running on localhost).
fn client_ip(headers: &HeaderMap) -> IpAddr {
    if let Some(v) = headers.get("x-forwarded-for")
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

fn format_set_cookie(value: &str, max_age: u64) -> String {
    // `Secure` is conditional in the spec; we omit it here because
    // bare-HTTP deployments are explicitly supported (per "Open
    // questions" in auth-and-authz.md). Operators behind TLS can
    // proxy through a reverse proxy that rewrites the header.
    format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}",)
}

const ENABLED_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>WireMirage — Sign in</title>
<style>
body { font-family: system-ui, sans-serif; max-width: 28rem; margin: 4rem auto; padding: 0 1rem; }
label { display: block; margin: 0.75rem 0 0.25rem; font-weight: 600; }
input { width: 100%; padding: 0.5rem; box-sizing: border-box; font-size: 1rem; }
button { margin-top: 1rem; padding: 0.5rem 1rem; font-size: 1rem; }
small { color: #666; }
</style>
</head>
<body>
<h1>Sign in</h1>
<form method="post" action="/__auth/login/password">
<label for="username">Username</label>
<input id="username" name="username" autocomplete="username" required>
<label for="password">Password</label>
<input id="password" name="password" type="password" autocomplete="current-password" required>
<button type="submit">Sign in</button>
</form>
<p><small>Local-account login. Configured users only.</small></p>
</body>
</html>
"#;

const DISABLED_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>WireMirage — Sign in</title>
</head>
<body>
<h1>No login methods configured</h1>
<p>This WireMirage host has no login methods enabled. Set <code>WM_LOCAL_AUTH</code> (and <code>SESSION_SECRET</code>) to enable local password login, or configure an OAuth provider.</p>
</body>
</html>
"#;

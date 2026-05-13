//! Web UI routes under `/__ui/*` (slice 21).
//!
//! The UI is for inspection + token management + a small set of
//! admin actions — handlers are deliberately *not* the primary place
//! to author mocks (agents and the API are). See `web-ui-design.md`.
//!
//! Templates are embedded with `include_str!` at compile time so
//! `cargo build` produces a single binary. minijinja parses them once
//! at startup; rendering is cheap.
//!
//! Auth on `/__ui/*`: a middleware (`require_session`) redirects
//! unauthenticated browsers to `/__auth/login?next=...`. Inside the
//! handler we reuse the standard `AuthContext` extractor since the
//! middleware has already established a valid session.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use minijinja::Environment;
use minijinja::context;
use serde::Serialize;

use crate::AppState;
use crate::auth::AuthContext;

pub mod auth_redirect;
pub mod csrf;
pub mod static_assets;

/// Process-wide template environment. Built once at startup; cloning
/// is cheap (the inner Arc is shared).
#[derive(Clone)]
pub struct UiTemplates {
    env: Arc<Environment<'static>>,
}

impl UiTemplates {
    pub fn new() -> Self {
        let mut env = Environment::new();
        // One `add_template` call per file. Macro-driven so a new
        // template is one line at the call site — easy to keep in
        // sync as new screens land in subsequent slices.
        macro_rules! tmpl {
            ($name:literal, $path:literal) => {
                env.add_template($name, include_str!($path))
                    .expect("template parses at compile time");
            };
        }
        tmpl!("base.html", "templates/base.html");
        tmpl!("login.html", "templates/login.html");
        tmpl!("home.html", "templates/home.html");
        tmpl!("placeholder.html", "templates/placeholder.html");
        Self { env: Arc::new(env) }
    }

    pub fn render<S: Serialize>(&self, name: &str, ctx: S) -> Result<String, minijinja::Error> {
        let tmpl = self.env.get_template(name)?;
        tmpl.render(ctx)
    }
}

impl Default for UiTemplates {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `/__ui/*` sub-router. Mounted from `server::router`.
/// Takes state by value so the auth-redirect middleware can hold a
/// clone — the merged router from `server::router` provides the same
/// state to inner handlers separately.
pub fn router(state: AppState) -> Router {
    let authed: Router<AppState> = Router::new()
        .route("/__ui/", get(home))
        .route("/__ui/groups", get(stub_groups_index))
        .route("/__ui/groups/{group}", get(stub_group_detail))
        .route("/__ui/groups/{group}/state", get(stub_group_state))
        .route("/__ui/routes", get(stub_routes_index))
        .route("/__ui/routes/new", get(stub_routes_new))
        .route("/__ui/routes/{group}/{number}", get(stub_route_detail))
        .route("/__ui/routes/{group}/{number}/state", get(stub_route_state))
        .route("/__ui/journal/live", get(stub_journal_live))
        .route("/__ui/journal/{group}/{number}", get(stub_journal_entry))
        .route("/__ui/unmatched", get(stub_unmatched))
        .route("/__ui/me/tokens", get(stub_tokens))
        .route("/__ui/settings", get(stub_settings))
        .route("/__ui/admin/health", get(stub_admin_health))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_redirect::require_session,
        ));

    let public: Router<AppState> = Router::new()
        // Static assets are deliberately *not* behind auth — CSS for
        // the login page would chicken-and-egg otherwise.
        .route("/__ui/static/{*path}", get(static_assets::serve));

    authed.merge(public).with_state(state)
}

// -- Handlers ---------------------------------------------------------------

async fn home(State(state): State<AppState>, auth: AuthContext) -> Response {
    // List groups visible to the caller. Admin sees all; non-admin
    // sees their own. Same rule as the REST /__api/groups endpoint.
    let groups = if auth.is_admin {
        state.routes().registry().list_groups()
    } else {
        state
            .routes()
            .registry()
            .list_groups_by_owner(&auth.user_id)
    };
    let groups = match groups {
        Ok(g) => g,
        Err(e) => return ui_error_500(&state, &auth, format!("registry: {e}")),
    };

    // Convert into a minimal struct minijinja can iterate over —
    // template needs name, last_activity, route_count.
    let all_routes = state.routes().registry().list_routes().unwrap_or_default();
    let mut visible: Vec<HomeGroupRow> = groups
        .into_iter()
        .map(|g| {
            let route_count = all_routes.iter().filter(|r| r.group_id == g.id).count();
            HomeGroupRow {
                name: g.name,
                last_activity: g.last_activity_at.map(|t| t.to_rfc3339()),
                route_count,
                ttl_seconds: g.ttl_seconds,
            }
        })
        .collect();
    visible.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

    render(
        &state,
        "home.html",
        context! {
            page_title => "Home",
            user => UserBadge::from(&auth),
            groups => visible,
        },
    )
}

#[derive(Serialize)]
struct HomeGroupRow {
    name: String,
    last_activity: Option<String>,
    route_count: usize,
    ttl_seconds: u64,
}

#[derive(Serialize)]
pub(crate) struct UserBadge {
    pub name: String,
    pub is_admin: bool,
}

impl UserBadge {
    pub fn from(auth: &AuthContext) -> Self {
        Self {
            name: auth.user_name.clone(),
            is_admin: auth.is_admin,
        }
    }
}

// -- Stubs ------------------------------------------------------------------
//
// Each stub names what'll eventually live at this URL, plus the
// equivalent API path so the user can poke at the data via the CLI
// or curl until the real page lands. Stubs share a single template
// to keep the code DRY.

async fn stub_groups_index(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(&state, &auth, "Groups", "GET /__api/groups")
}

async fn stub_group_detail(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group): Path<String>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Group: {group}"),
        &format!("GET /__api/groups/{group}"),
    )
}

async fn stub_group_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group): Path<String>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("State: {group}"),
        // No REST endpoint for group-wide state listing yet; the
        // user can drop to per-route state.
        "DELETE /__api/groups/{group}/state to wipe",
    )
}

async fn stub_routes_index(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(&state, &auth, "Routes", "GET /__api/routes")
}

async fn stub_routes_new(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(&state, &auth, "New route", "POST /__api/routes")
}

async fn stub_route_detail(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Route: {group}/{number}"),
        &format!("GET /__api/routes/{group}/{number}"),
    )
}

async fn stub_route_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Route state: {group}/{number}"),
        &format!("GET /__api/routes/{group}/{number}/state"),
    )
}

async fn stub_journal_live(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(
        &state,
        &auth,
        "Live journal",
        "GET /__api/journal/tail (SSE)",
    )
}

async fn stub_journal_entry(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Journal: {group}/{number}"),
        &format!("GET /__api/journal/{group}/{number}"),
    )
}

async fn stub_unmatched(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(&state, &auth, "Unmatched", "GET /__api/unmatched")
}

async fn stub_tokens(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(&state, &auth, "My tokens", "GET /__api/tokens")
}

async fn stub_settings(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(
        &state,
        &auth,
        "Settings",
        "GET /__api/users + /__api/tokens",
    )
}

async fn stub_admin_health(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(&state, &auth, "Admin health", "GET /__health and /__ready")
}

fn stub(state: &AppState, auth: &AuthContext, title: &str, api_hint: &str) -> Response {
    render(
        state,
        "placeholder.html",
        context! {
            page_title => title,
            api_hint => api_hint,
            user => UserBadge::from(auth),
        },
    )
}

fn forbidden_page(state: &AppState, auth: &AuthContext) -> Response {
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Forbidden",
            api_hint => "admin role required",
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp
}

// -- Render helpers ---------------------------------------------------------

fn render<S: Serialize>(state: &AppState, template: &str, ctx: S) -> Response {
    match state.ui_templates().render(template, ctx) {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            tracing::error!(template, error = %e, "template render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template render failed: {e}"),
            )
                .into_response()
        }
    }
}

fn ui_error_500(state: &AppState, auth: &AuthContext, msg: String) -> Response {
    tracing::error!("ui internal error: {msg}");
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Something went wrong",
            api_hint => msg,
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp
}
